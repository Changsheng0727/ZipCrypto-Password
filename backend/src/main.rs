use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use flate2::read::DeflateDecoder;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    env,
    io::{Read, Seek, SeekFrom},
    path::{Path as FilePath, PathBuf},
    process::Stdio,
    sync::{Arc, RwLock},
};
use tokio::{fs, process::Command, sync::mpsc};
use uuid::Uuid;

const MAX_WORKERS: usize = 16;

#[derive(Clone)]
struct AppState {
    jobs: Arc<RwLock<HashMap<Uuid, JobState>>>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct JobState {
    id: Uuid,
    status: JobStatus,
    phase: String,
    progress: u8,
    message: String,
    logs: Vec<LogEntry>,
    keys: Option<String>,
    output_directory: Option<String>,
    created_at: DateTime<Utc>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "lowercase")]
enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LogEntry {
    at: DateTime<Utc>,
    level: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartRequest {
    archive_path: String,
    output_directory: Option<String>,
    worker_count: Option<usize>,
    known_header: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StartResponse {
    id: Uuid,
}

#[tokio::main]
async fn main() {
    let state = AppState {
        jobs: Arc::new(RwLock::new(HashMap::new())),
    };
    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/jobs", post(start_job))
        .route("/api/jobs/:id", get(get_job))
        .route("/api/jobs/:id/cancel", post(cancel_job))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8787")
        .await
        .expect("bind backend listener");
    println!("zip-unlocker backend listening on http://127.0.0.1:8787");
    axum::serve(listener, app).await.expect("serve backend");
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true, "service": "zip-unlocker" }))
}

async fn start_job(
    State(state): State<AppState>,
    Json(request): Json<StartRequest>,
) -> Result<Json<StartResponse>, ApiError> {
    let archive_path = PathBuf::from(request.archive_path.trim());
    if !archive_path.is_file() {
        return Err(ApiError::bad_request("找不到待处理的 ZIP 文件。"));
    }
    let job_id = Uuid::new_v4();
    let initial_state = JobState {
        id: job_id,
        status: JobStatus::Queued,
        phase: "排队中".into(),
        progress: 0,
        message: "任务已创建，等待开始。".into(),
        logs: vec![log("info", "已创建本地解密任务。")],
        keys: None,
        output_directory: None,
        created_at: Utc::now(),
    };
    state.jobs.write().unwrap().insert(job_id, initial_state);

    let jobs = state.jobs.clone();
    tokio::spawn(async move {
        run_job(
            jobs,
            job_id,
            archive_path,
            request.output_directory.map(PathBuf::from),
            request.worker_count.unwrap_or_else(default_worker_count),
            request
                .known_header
                .unwrap_or_else(|| "504b0304140000000800".into()),
        )
        .await;
    });
    Ok(Json(StartResponse { id: job_id }))
}

async fn get_job(
    State(state): State<AppState>,
    Path(job_id): Path<Uuid>,
) -> Result<Json<JobState>, ApiError> {
    state
        .jobs
        .read()
        .unwrap()
        .get(&job_id)
        .cloned()
        .map(Json)
        .ok_or_else(|| ApiError::not_found("找不到该任务。"))
}

async fn cancel_job(
    State(state): State<AppState>,
    Path(job_id): Path<Uuid>,
) -> Result<Json<JobState>, ApiError> {
    update_job(&state.jobs, job_id, |job| {
        if matches!(job.status, JobStatus::Queued | JobStatus::Running) {
            job.status = JobStatus::Cancelled;
            job.phase = "已取消".into();
            job.message = "任务已标记为取消；当前外部进程完成后将停止后续步骤。".into();
            job.logs.push(log("warning", "用户请求取消任务。"));
        }
    });
    get_job(State(state), Path(job_id)).await
}

async fn run_job(
    jobs: Arc<RwLock<HashMap<Uuid, JobState>>>,
    job_id: Uuid,
    archive_path: PathBuf,
    requested_output_directory: Option<PathBuf>,
    requested_worker_count: usize,
    known_header: String,
) {
    let output_directory = requested_output_directory.unwrap_or_else(|| {
        archive_path
            .parent()
            .unwrap_or_else(|| FilePath::new("."))
            .join("解密结果")
    });
    let worker_count = requested_worker_count.clamp(1, MAX_WORKERS);
    update_job(&jobs, job_id, |job| {
        job.status = JobStatus::Running;
        job.phase = "准备中".into();
        job.progress = 4;
        job.message = format!("使用 {worker_count} 个并发线程，所有文件留在本机处理。");
        job.logs.push(log(
            "info",
            &format!("输入归档：{}", archive_path.display()),
        ));
    });

    if let Err(error_message) = fs::create_dir_all(&output_directory).await {
        finish_with_error(&jobs, job_id, format!("无法创建输出目录：{error_message}"));
        return;
    }
    let tool_path = resolve_bkcrack_path();
    if !tool_path.is_file() {
        finish_with_error(
            &jobs,
            job_id,
            format!("找不到 bkcrack.exe：{}", tool_path.display()),
        );
        return;
    }
    let archive_layout = match inspect_archive_layout(&archive_path) {
        Ok(layout) => layout,
        Err(error_message) => {
            finish_with_error(&jobs, job_id, error_message);
            return;
        }
    };
    update_job(&jobs, job_id, |job| {
        job.phase = "恢复密钥".into();
        job.progress = 10;
        job.logs.push(log(
            "info",
            &format!(
                "已识别 A 题条目：未压缩 {} 字节，明文偏移 {}。",
                archive_layout.uncompressed_size, archive_layout.known_plaintext_offset
            ),
        ));
        job.logs.push(log(
            "info",
            "先尝试 ZIP 本地头 10 字节已知明文锚点，搜索空间更小。",
        ));
    });

    let known_plaintext_path = output_directory.join(".known_header.bin");
    if let Err(error_message) = write_hex_file(&known_plaintext_path, &known_header).await {
        finish_with_error(&jobs, job_id, error_message);
        return;
    }
    let attack_output = match run_bkcrack_attack(
        &tool_path,
        &archive_path,
        &known_plaintext_path,
        worker_count,
        &archive_layout,
    )
    .await
    {
        Ok(output) => output,
        Err(error_message) => {
            finish_with_error(&jobs, job_id, error_message);
            let _ = fs::remove_file(known_plaintext_path).await;
            return;
        }
    };
    if is_job_cancelled(&jobs, job_id) {
        let _ = fs::remove_file(known_plaintext_path).await;
        return;
    }
    let keys = match parse_keys(&attack_output) {
        Some(keys) => keys,
        None => {
            finish_with_error(
                &jobs,
                job_id,
                "未找到密钥。请确认这是 ZipCrypto，并检查已知明文锚点。".into(),
            );
            let _ = fs::remove_file(known_plaintext_path).await;
            return;
        }
    };
    update_job(&jobs, job_id, |job| {
        job.phase = "并发解密".into();
        job.progress = 25;
        job.keys = Some(keys.clone());
        job.logs
            .push(log("success", &format!("恢复内部密钥：{keys}")));
    });

    let decrypt_result = decrypt_entries(
        &tool_path,
        &archive_path,
        &output_directory,
        &keys,
        &jobs,
        job_id,
    )
    .await;
    let _ = fs::remove_file(known_plaintext_path).await;
    if let Err(error_message) = decrypt_result {
        finish_with_error(&jobs, job_id, error_message);
        return;
    }
    if is_job_cancelled(&jobs, job_id) {
        return;
    }
    if let Err(error_message) = inflate_deflate_entry(&output_directory).await {
        finish_with_error(&jobs, job_id, error_message);
        return;
    }
    update_job(&jobs, job_id, |job| {
        job.status = JobStatus::Completed;
        job.phase = "完成".into();
        job.progress = 100;
        job.message = format!(
            "已解密并校验 A-F 条目，结果位于 {}。",
            output_directory.display()
        );
        job.output_directory = Some(output_directory.display().to_string());
        job.logs.push(log("success", "全部条目处理完成。"));
    });
}

async fn run_bkcrack_attack(
    tool_path: &FilePath,
    archive_path: &FilePath,
    known_plaintext_path: &FilePath,
    worker_count: usize,
    archive_layout: &ArchiveLayout,
) -> Result<String, String> {
    let output = Command::new(tool_path)
        .args([
            "-j",
            &worker_count.to_string(),
            "-C",
            archive_path.to_string_lossy().as_ref(),
            "-c",
            "中文题目/A题.zip",
            "-p",
            known_plaintext_path.to_string_lossy().as_ref(),
            "-o",
            &archive_layout.known_plaintext_offset.to_string(),
            "-x",
            "0",
            "504b0304",
            "-x",
            &archive_layout.trailing_zero_offset.to_string(),
            "0000",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| format!("启动 bkcrack 失败：{error}"))?;
    let mut combined_output = String::from_utf8_lossy(&output.stdout).into_owned();
    combined_output.push_str(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        return Err(format!("bkcrack 攻击失败：{}", tail(&combined_output, 800)));
    }
    Ok(combined_output)
}

async fn decrypt_entries(
    tool_path: &FilePath,
    archive_path: &FilePath,
    output_directory: &FilePath,
    keys: &str,
    jobs: &Arc<RwLock<HashMap<Uuid, JobState>>>,
    job_id: Uuid,
) -> Result<(), String> {
    let entry_definitions = [
        ("A题.zip", "A题.zip"),
        ("B题.zip", "B题.zip"),
        ("C题.zip", "C题.zip"),
        ("D题.zip", "D题.zip"),
        ("E题.zip", "E题.zip"),
        ("F题.zip", "F题.deflate"),
    ];
    let archive_path_text = archive_path.to_string_lossy().to_string();
    let output_directory_text = output_directory.to_string_lossy().to_string();
    let keys_text = keys.to_string();
    let (sender, mut receiver) = mpsc::channel::<Result<(String, String), String>>(6);
    for (entry_name, output_name) in entry_definitions {
        let sender = sender.clone();
        let tool_path = tool_path.to_path_buf();
        let archive_path_text = archive_path_text.clone();
        let output_directory_text = output_directory_text.clone();
        let keys_text = keys_text.clone();
        tokio::spawn(async move {
            let output_path = FilePath::new(&output_directory_text).join(output_name);
            let command_result = Command::new(tool_path)
                .args([
                    "-C",
                    &archive_path_text,
                    "-c",
                    &format!("中文题目/{entry_name}"),
                    "-k",
                    keys_text.split_whitespace().next().unwrap_or_default(),
                    keys_text.split_whitespace().nth(1).unwrap_or_default(),
                    keys_text.split_whitespace().nth(2).unwrap_or_default(),
                    "-d",
                    output_path.to_string_lossy().as_ref(),
                ])
                .output()
                .await;
            let result = command_result
                .map_err(|error| format!("{entry_name} 启动失败：{error}"))
                .and_then(|output| {
                    if output.status.success() {
                        Ok((entry_name.into(), output_path.display().to_string()))
                    } else {
                        Err(format!(
                            "{entry_name} 解密失败：{}",
                            tail(&String::from_utf8_lossy(&output.stderr), 500)
                        ))
                    }
                });
            let _ = sender.send(result).await;
        });
    }
    drop(sender);
    let mut completed_count = 0u8;
    while let Some(result) = receiver.recv().await {
        match result {
            Ok((entry_name, output_path)) => {
                completed_count += 1;
                let progress = 25 + completed_count * 10;
                update_job(jobs, job_id, |job| {
                    job.progress = progress;
                    job.logs.push(log(
                        "success",
                        &format!("{entry_name} 已解密：{output_path}"),
                    ));
                });
            }
            Err(error_message) => return Err(error_message),
        }
    }
    Ok(())
}

async fn inflate_deflate_entry(output_directory: &FilePath) -> Result<(), String> {
    let compressed_path = output_directory.join("F题.deflate");
    if !compressed_path.is_file() {
        return Err("F 题解密结果不存在，无法进行 Deflate 膨胀。".into());
    }
    let compressed_bytes = fs::read(&compressed_path)
        .await
        .map_err(|error| format!("读取 F 题 Deflate 数据失败：{error}"))?;
    let mut decoder = DeflateDecoder::new(compressed_bytes.as_slice());
    let mut uncompressed_bytes = Vec::new();
    decoder
        .read_to_end(&mut uncompressed_bytes)
        .map_err(|error| format!("F 题 Deflate 膨胀失败：{error}"))?;
    fs::write(output_directory.join("F题.zip"), uncompressed_bytes)
        .await
        .map_err(|error| format!("写入 F 题 ZIP 失败：{error}"))?;
    fs::remove_file(compressed_path)
        .await
        .map_err(|error| format!("清理 F 题中间文件失败：{error}"))
}

struct ArchiveLayout {
    uncompressed_size: u64,
    known_plaintext_offset: u64,
    trailing_zero_offset: u64,
}

fn inspect_archive_layout(archive_path: &FilePath) -> Result<ArchiveLayout, String> {
    let mut archive_file =
        std::fs::File::open(archive_path).map_err(|error| format!("打开归档失败：{error}"))?;
    let scan_limit = archive_file
        .metadata()
        .map_err(|error| format!("读取归档元数据失败：{error}"))?
        .len()
        .min(64 * 1024 * 1024);
    let mut archive_offset = 0u64;
    while archive_offset + 30 <= scan_limit {
        archive_file
            .seek(SeekFrom::Start(archive_offset))
            .map_err(|error| format!("扫描归档失败：{error}"))?;
        let mut local_header = [0u8; 30];
        archive_file
            .read_exact(&mut local_header)
            .map_err(|error| format!("读取 ZIP 本地头失败：{error}"))?;
        if &local_header[0..4] == b"PK\x03\x04" {
            let filename_length = u16::from_le_bytes([local_header[26], local_header[27]]) as usize;
            let extra_length = u16::from_le_bytes([local_header[28], local_header[29]]) as usize;
            let mut filename = vec![0u8; filename_length];
            archive_file
                .read_exact(&mut filename)
                .map_err(|error| format!("读取 ZIP 文件名失败：{error}"))?;
            let filename_text = String::from_utf8_lossy(&filename);
            if filename_text.ends_with("/A题.zip") || filename_text == "A题.zip" {
                let uncompressed_size = u32::from_le_bytes([
                    local_header[22],
                    local_header[23],
                    local_header[24],
                    local_header[25],
                ]) as u64;
                if uncompressed_size < 22 {
                    return Err("A 题条目的未压缩大小异常。".into());
                }
                return Ok(ArchiveLayout {
                    uncompressed_size,
                    known_plaintext_offset: uncompressed_size - 22,
                    trailing_zero_offset: uncompressed_size - 2,
                });
            }
            archive_offset =
                archive_offset.saturating_add(30 + filename_length as u64 + extra_length as u64);
            continue;
        }
        archive_offset += 1;
    }
    Err("未找到名为 A 题的 ZIP 条目，当前恢复流程需要包含 A 题条目。".into())
}

fn resolve_bkcrack_path() -> PathBuf {
    if let Some(configured_path) = env::var_os("BKCRACK_PATH") {
        return PathBuf::from(configured_path);
    }
    let candidate_paths = [
        PathBuf::from("..\\tools\\bkcrack.exe"),
        PathBuf::from("..\\..\\..\\bkcrack\\bkcrack-1.8.1-win64\\bkcrack.exe"),
        PathBuf::from("..\\..\\bkcrack\\bkcrack-1.8.1-win64\\bkcrack.exe"),
        PathBuf::from("bkcrack\\bkcrack-1.8.1-win64\\bkcrack.exe"),
    ];
    candidate_paths
        .into_iter()
        .find(|candidate_path| candidate_path.is_file())
        .unwrap_or_else(|| PathBuf::from("bkcrack.exe"))
}

async fn write_hex_file(path: &FilePath, hexadecimal_data: &str) -> Result<(), String> {
    if hexadecimal_data.is_empty() || hexadecimal_data.len() % 2 != 0 {
        return Err("已知明文必须是非空、偶数长度的十六进制字符串。".into());
    }
    let bytes = (0..hexadecimal_data.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hexadecimal_data[index..index + 2], 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "已知明文必须是偶数长度的十六进制字符串。".to_string())?;
    fs::write(path, bytes)
        .await
        .map_err(|error| format!("无法写入临时明文文件：{error}"))
}

fn parse_keys(output: &str) -> Option<String> {
    output.lines().rev().find_map(|line| {
        let candidate = line.trim();
        let pieces: Vec<_> = candidate.split_whitespace().collect();
        (pieces.len() == 3
            && pieces.iter().all(|piece| {
                piece.len() == 8 && piece.chars().all(|character| character.is_ascii_hexdigit())
            }))
        .then(|| pieces.join(" "))
    })
}

fn update_job(
    jobs: &Arc<RwLock<HashMap<Uuid, JobState>>>,
    job_id: Uuid,
    update: impl FnOnce(&mut JobState),
) {
    if let Some(job) = jobs.write().unwrap().get_mut(&job_id) {
        update(job);
    }
}

fn is_job_cancelled(jobs: &Arc<RwLock<HashMap<Uuid, JobState>>>, job_id: Uuid) -> bool {
    jobs.read()
        .unwrap()
        .get(&job_id)
        .map(|job| matches!(job.status, JobStatus::Cancelled))
        .unwrap_or(true)
}

fn finish_with_error(
    jobs: &Arc<RwLock<HashMap<Uuid, JobState>>>,
    job_id: Uuid,
    error_message: String,
) {
    update_job(jobs, job_id, |job| {
        job.status = JobStatus::Failed;
        job.phase = "失败".into();
        job.message = error_message.clone();
        job.logs.push(log("error", &error_message));
    });
}

fn log(level: &str, message: &str) -> LogEntry {
    LogEntry {
        at: Utc::now(),
        level: level.into(),
        message: message.into(),
    }
}

fn tail(text: &str, maximum_characters: usize) -> String {
    text.chars()
        .rev()
        .take(maximum_characters)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

fn default_worker_count() -> usize {
    num_cpus::get().clamp(1, MAX_WORKERS)
}

struct ApiError(StatusCode, String);

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, message.into())
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self(StatusCode::NOT_FOUND, message.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}
