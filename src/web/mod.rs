use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Html,
    Json,
};
use serde::Serialize;
use serde_json::Value;

use crate::diagnose;
use crate::eval::types::SessionView;
use crate::grader;
use crate::grader::types::GradeReport;
use crate::probe;
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
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnose_summary: Option<DiagnoseSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    probe_summary: Option<ProbeSummary>,
}

#[derive(Serialize)]
struct ProbeSummary {
    total_findings: usize,
    high: usize,
    medium: usize,
    low: usize,
}

#[derive(Serialize)]
struct DiagnoseSummary {
    total_issues: usize,
    errors: usize,
    warnings: usize,
    infos: usize,
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
                let diag = read_diagnose_summary(&state.log_dir, sid);
                let prb = read_probe_summary(&state.log_dir, sid);
                summaries.push(SessionSummary {
                    session_id: sid.clone(),
                    model: report.model,
                    turn_count: report.turn_count,
                    jsonl_ids: report.jsonl_ids.clone(),
                    overall: Some(report.overall),
                    graded: true,
                    dimensions: dims,
                    diagnose_summary: diag,
                    probe_summary: prb,
                });
                continue;
            }
        }

        // No grade yet — try view
        if let Ok(view_json) = std::fs::read_to_string(&view_path) {
            if let Ok(view) = serde_json::from_str::<SessionView>(&view_json) {
                let diag = read_diagnose_summary(&state.log_dir, sid);
                let prb = read_probe_summary(&state.log_dir, sid);
                summaries.push(SessionSummary {
                    session_id: sid.clone(),
                    model: view.model,
                    turn_count: view.turns.len(),
                    jsonl_ids: view.jsonl_ids.clone(),
                    overall: None,
                    graded: false,
                    dimensions: Vec::new(),
                    diagnose_summary: diag,
                    probe_summary: prb,
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

pub async fn grade_session(
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

    let report = grader::run_pipeline(&view, &state.grader_config).await;

    // Write grade.json
    if let Ok(json) = serde_json::to_string_pretty(&report) {
        if let Ok(mut file) = std::fs::File::create(&grade_path) {
            use std::io::Write;
            file.write_all(json.as_bytes()).ok();
        }
    }

    Ok(Json(serde_json::json!({
        "view": view,
        "grade": report,
    })))
}

fn is_safe_session_id(s: &str) -> bool {
    !s.contains("..") && !s.contains('/') && !s.contains('\\')
}

fn read_diagnose_summary(log_dir: &str, session_id: &str) -> Option<DiagnoseSummary> {
    let path = format!("{}/{}.diagnose.json", log_dir, session_id);
    let json = std::fs::read_to_string(&path).ok()?;
    let report: diagnose::types::DiagnoseReport = serde_json::from_str(&json).ok()?;
    Some(DiagnoseSummary {
        total_issues: report.summary.total_issues,
        errors: report.summary.errors,
        warnings: report.summary.warnings,
        infos: report.summary.infos,
    })
}

fn read_probe_summary(log_dir: &str, session_id: &str) -> Option<ProbeSummary> {
    let path = format!("{}/{}.probe.json", log_dir, session_id);
    let json = std::fs::read_to_string(&path).ok()?;
    let report: probe::types::ProbeReport = serde_json::from_str(&json).ok()?;
    let summary = probe::types::ProbeSummary::from_report(&report);
    Some(ProbeSummary {
        total_findings: summary.total_findings,
        high: summary.high,
        medium: summary.medium,
        low: summary.low,
    })
}

// ── Diagnose handlers ──

pub async fn diagnose_session(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if !is_safe_session_id(&session_id) {
        return Err((StatusCode::BAD_REQUEST, "invalid session_id".into()));
    }

    let report = diagnose::run(&session_id, &state.log_dir, &state.grader_config)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(serde_json::to_value(&report).unwrap_or_default()))
}

pub async fn get_diagnose(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if !is_safe_session_id(&session_id) {
        return Err((StatusCode::BAD_REQUEST, "invalid session_id".into()));
    }

    let report = diagnose::read_existing(&session_id, &state.log_dir)
        .map_err(|e| (StatusCode::NOT_FOUND, e))?;

    Ok(Json(serde_json::to_value(&report).unwrap_or_default()))
}

#[derive(serde::Deserialize)]
pub struct RawQuery {
    pub ids: Option<String>,
}

pub async fn get_raw_jsonl(
    State(state): State<Arc<AppState>>,
    Path(jsonl_stem): Path<String>,
    axum::extract::Query(query): axum::extract::Query<RawQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if !is_safe_session_id(&jsonl_stem) {
        return Err((StatusCode::BAD_REQUEST, "invalid jsonl_stem".into()));
    }

    let ids: Vec<u64> = match &query.ids {
        Some(s) => s
            .split(',')
            .filter_map(|p| p.trim().parse::<u64>().ok())
            .collect(),
        None => Vec::new(),
    };

    let entries = diagnose::read_raw_jsonl(&jsonl_stem, &ids, &state.log_dir)
        .map_err(|e| (StatusCode::NOT_FOUND, e))?;

    Ok(Json(serde_json::json!({ "entries": entries })))
}

// ── Probe handlers ──

pub async fn probe_session(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if !is_safe_session_id(&session_id) {
        return Err((StatusCode::BAD_REQUEST, "invalid session_id".into()));
    }

    let report = probe::run(&session_id, &state.log_dir, &state.probe_config, &state.grader_config)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(serde_json::to_value(&report).unwrap_or_default()))
}

pub async fn get_probe(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if !is_safe_session_id(&session_id) {
        return Err((StatusCode::BAD_REQUEST, "invalid session_id".into()));
    }

    let report = probe::read_existing(&session_id, &state.log_dir)
        .map_err(|e| (StatusCode::NOT_FOUND, e))?;

    Ok(Json(serde_json::to_value(&report).unwrap_or_default()))
}
