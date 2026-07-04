mod auth;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use futures_util::StreamExt;
use reqwest::{Client, StatusCode, cookie::CookieStore, cookie::Jar, header};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{Emitter, Manager};
use tokio::{fs, io::AsyncWriteExt, sync::Semaphore};
use url::Url;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};

use auth::{QrLoginController, client_with_jar, spawn_qr_login};

const CANVAS_ORIGIN: &str = "https://oc.sjtu.edu.cn";
const VIDEO_API: &str = "https://v.sjtu.edu.cn/jy-application-canvas-sjtu";
const VIDEO_REFERER: &str = "https://courses.sjtu.edu.cn";
const DEFAULT_DOWNLOAD_CONCURRENCY: usize = 16;
const MAX_DOWNLOAD_CONCURRENCY: usize = 64;

struct DownloadConcurrency {
    limit: usize,
    slots: Arc<Semaphore>,
}

struct AppState {
    session: Mutex<Option<VideoSession>>,
    download_concurrency: Mutex<DownloadConcurrency>,
    cookie_jar: Mutex<Arc<Jar>>,
    qr_login: Mutex<Option<QrLoginController>>,
    downloads: Mutex<HashMap<String, ActiveDownload>>,
}

#[derive(Clone)]
struct TaskControl {
    paused: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

impl TaskControl {
    fn new() -> Self {
        Self {
            paused: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
    }

    fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
        self.cancelled.store(false, Ordering::Relaxed);
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.paused.store(false, Ordering::Relaxed);
    }

    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

#[derive(Clone)]
struct DownloadTaskMeta {
    session: VideoSession,
    lesson_id: String,
    lesson_title: String,
    begin_time: String,
    output_dir: PathBuf,
    cdvi_view_num: i64,
    signal: String,
}

struct ActiveDownload {
    meta: DownloadTaskMeta,
    control: TaskControl,
    progress: DownloadProgress,
    running: Arc<AtomicBool>,
}

impl AppState {
    fn new() -> Self {
        Self {
            session: Mutex::new(None),
            download_concurrency: Mutex::new(DownloadConcurrency {
                limit: DEFAULT_DOWNLOAD_CONCURRENCY,
                slots: Arc::new(Semaphore::new(DEFAULT_DOWNLOAD_CONCURRENCY)),
            }),
            cookie_jar: Mutex::new(Arc::new(Jar::default())),
            qr_login: Mutex::new(None),
            downloads: Mutex::new(HashMap::new()),
        }
    }

    fn download_slots(&self) -> Result<Arc<Semaphore>, String> {
        self.download_concurrency
            .lock()
            .map_err(|_| "下载并发设置不可用".to_string())
            .map(|guard| guard.slots.clone())
    }

    fn download_concurrency_limit(&self) -> Result<usize, String> {
        self.download_concurrency
            .lock()
            .map_err(|_| "下载并发设置不可用".to_string())
            .map(|guard| guard.limit)
    }

    fn set_download_concurrency(&self, limit: usize) -> Result<usize, String> {
        let limit = limit.clamp(1, MAX_DOWNLOAD_CONCURRENCY);
        let mut guard = self
            .download_concurrency
            .lock()
            .map_err(|_| "下载并发设置不可用".to_string())?;
        guard.limit = limit;
        guard.slots = Arc::new(Semaphore::new(limit));
        Ok(limit)
    }

    fn register_download(&self, task_id: String, download: ActiveDownload) -> Result<(), String> {
        let mut guard = self.downloads.lock().map_err(|_| "下载任务状态不可用")?;
        guard.insert(task_id, download);
        Ok(())
    }

    fn record_progress(&self, progress: &DownloadProgress) {
        if let Ok(mut guard) = self.downloads.lock() {
            if let Some(task) = guard.get_mut(&progress.task_id) {
                task.progress = progress.clone();
            }
        }
    }

    fn get_download(&self, task_id: &str) -> Result<(DownloadTaskMeta, TaskControl, DownloadProgress, Arc<AtomicBool>), String> {
        let guard = self.downloads.lock().map_err(|_| "下载任务状态不可用")?;
        let task = guard.get(task_id).ok_or_else(|| "找不到下载任务".to_string())?;
        Ok((
            DownloadTaskMeta {
                session: task.meta.session.clone(),
                lesson_id: task.meta.lesson_id.clone(),
                lesson_title: task.meta.lesson_title.clone(),
                begin_time: task.meta.begin_time.clone(),
                output_dir: task.meta.output_dir.clone(),
                cdvi_view_num: task.meta.cdvi_view_num,
                signal: task.meta.signal.clone(),
            },
            TaskControl {
                paused: task.control.paused.clone(),
                cancelled: task.control.cancelled.clone(),
            },
            task.progress.clone(),
            task.running.clone(),
        ))
    }

    fn cookie_jar(&self) -> Result<Arc<Jar>, String> {
        self.cookie_jar
            .lock()
            .map_err(|_| "会话状态不可用".to_string())
            .map(|guard| guard.clone())
    }

    fn replace_cookie_jar(&self) -> Result<(), String> {
        let mut guard = self.cookie_jar.lock().map_err(|_| "会话状态不可用")?;
        *guard = Arc::new(Jar::default());
        Ok(())
    }

    fn stop_qr_login(&self) -> Result<(), String> {
        let mut guard = self.qr_login.lock().map_err(|_| "会话状态不可用")?;
        if let Some(mut controller) = guard.take() {
            controller.stop();
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct VideoSession {
    token: String,
    canvas_course_id: String,
    course_id: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccessParams {
    #[serde(default)]
    cour_id: String,
    #[serde(default)]
    lti_course_id: String,
    #[serde(default)]
    course_name: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuthStatus {
    phase: &'static str,
    message: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct Course {
    id: u64,
    name: String,
    #[serde(default)]
    course_code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    end_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workflow_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    enrollment_state: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct Lesson {
    video_id: String,
    title: String,
    begin_time: String,
    end_time: String,
    classroom: String,
    audit_status: i64,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DownloadRequest {
    lesson_id: String,
    lesson_title: String,
    begin_time: String,
    signals: Vec<String>,
    output_dir: Option<String>,
    course_name: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DownloadProgress {
    task_id: String,
    lesson_id: String,
    lesson_title: String,
    signal: String,
    file_name: String,
    stage: &'static str,
    downloaded: u64,
    total: Option<u64>,
    message: String,
}

#[derive(Deserialize)]
struct ApiEnvelope<T> {
    data: Option<T>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    status: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExchangeData {
    token: String,
    params: AccessParams,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct VideoDetail {
    #[serde(default)]
    video_play_response_vo_list: Vec<VideoTrack>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct VideoTrack {
    #[serde(default, deserialize_with = "deserialize_stringish")]
    id: String,
    #[serde(default)]
    cdvi_view_num: i64,
    #[serde(default, alias = "rtmpUrlHdv")]
    rtmp_url_hdv: String,
    #[serde(default, alias = "rtmpUrlHd")]
    rtmp_url_hd: String,
    #[serde(default, alias = "rtmpUrl")]
    rtmp_url: String,
}

fn track_download_url(track: &VideoTrack) -> &str {
    if !track.rtmp_url_hdv.trim().is_empty() {
        return track.rtmp_url_hdv.trim();
    }
    if !track.rtmp_url_hd.trim().is_empty() {
        return track.rtmp_url_hd.trim();
    }
    track.rtmp_url.trim()
}

fn deserialize_stringish<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::String(text) => Ok(text),
        Value::Number(number) => Ok(number.to_string()),
        Value::Null => Ok(String::new()),
        other => Ok(other.to_string()),
    }
}

fn api_error<T>(response: &ApiEnvelope<T>, context: &str) -> String {
    format!(
        "{}：{}{}",
        context,
        response.message.as_deref().unwrap_or("服务未返回数据"),
        response
            .status
            .map(|s| format!("（状态 {}）", s))
            .unwrap_or_default()
    )
}

fn parse_redirect_params(url: &str) -> HashMap<String, String> {
    let Ok(parsed) = Url::parse(url) else {
        return HashMap::new();
    };
    let mut params: HashMap<String, String> = parsed
        .query()
        .map(|query| url::form_urlencoded::parse(query.as_bytes()).into_owned().collect())
        .unwrap_or_default();
    if let Some(fragment) = parsed.fragment() {
        if let Some((_, query)) = fragment.split_once('?') {
            for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
                params.insert(key.into_owned(), value.into_owned());
            }
        }
    }
    params
}

fn form_inputs(form: scraper::ElementRef<'_>) -> HashMap<String, String> {
    let input_selector = Selector::parse("input").unwrap();
    form.select(&input_selector)
        .filter_map(|input| {
            let name = input.value().attr("name")?;
            let value = input.value().attr("value").unwrap_or_default();
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

async fn external_tool_id(client: &Client, course_id: u64) -> String {
    let default_id = "8329".to_string();
    let response = match client
        .get(format!("{CANVAS_ORIGIN}/courses/{course_id}"))
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => return default_id,
    };
    let html = match response.text().await {
        Ok(html) => html,
        Err(_) => return default_id,
    };
    let document = Html::parse_document(&html);
    let selector = match Selector::parse("div#main a") {
        Ok(selector) => selector,
        Err(_) => return default_id,
    };
    document
        .select(&selector)
        .find(|link| {
            link.text()
                .collect::<String>()
                .starts_with("课堂视频")
                && !link.text().collect::<String>().ends_with("旧版")
        })
        .and_then(|link| link.value().attr("href"))
        .map(|href| href.rsplit('/').next().unwrap_or("8329").to_string())
        .unwrap_or(default_id)
}

async fn authorize_video_session(
    jar: Arc<Jar>,
    course_id: u64,
) -> Result<VideoSession, String> {
    let client = http_client(jar.clone())?;
    let no_redirect = http_client_no_redirect(jar)?;
    let tool_id = external_tool_id(&client, course_id).await;
    let launch_html = client
        .get(format!(
            "{CANVAS_ORIGIN}/courses/{course_id}/external_tools/{tool_id}"
        ))
        .send()
        .await
        .map_err(|e| format!("打开课堂视频失败：{e}"))?
        .text()
        .await
        .map_err(|e| format!("读取课堂视频页面失败：{e}"))?;
    let launch_data = {
        let document = Html::parse_document(&launch_html);
        let form_selector = Selector::parse("form").map_err(|e| e.to_string())?;
        let launch_form = document
            .select(&form_selector)
            .find(|form| {
                form.value()
                    .attr("action")
                    .is_some_and(|action| action.contains("/oidc/login_initiations"))
            })
            .ok_or("未找到视频平台登录表单，可能是 Cookie 已失效，或课程页面结构已变化")?;
        form_inputs(launch_form)
    };

    let initiation_html = client
        .post(format!("{VIDEO_API}/oidc/login_initiations"))
        .form(&launch_data)
        .send()
        .await
        .map_err(|e| format!("视频平台登录失败：{e}"))?
        .text()
        .await
        .map_err(|e| format!("读取视频平台响应失败：{e}"))?;
    let auth_data = {
        let initiation_doc = Html::parse_document(&initiation_html);
        let form_selector = Selector::parse("form").map_err(|e| e.to_string())?;
        let auth_form = initiation_doc
            .select(&form_selector)
            .find(|form| {
                form.value()
                    .attr("action")
                    .is_some_and(|action| action.contains("/lti3/lti3Auth/ivs"))
            })
            .ok_or("未找到 LTI 鉴权表单，可能是登录状态失效，或学校视频平台返回流程已变化")?;
        form_inputs(auth_form)
    };

    let auth_response = no_redirect
        .post(format!("{VIDEO_API}/lti3/lti3Auth/ivs"))
        .form(&auth_data)
        .send()
        .await
        .map_err(|e| format!("LTI 鉴权失败：{e}"))?;
    let redirect = auth_response
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or_else(|| {
            if auth_response.status().is_redirection() {
                None
            } else {
                Some(auth_response.url().to_string())
            }
        })
        .ok_or("视频平台未返回跳转地址")?;
    let params = parse_redirect_params(&redirect);
    let token_id = params
        .get("tokenId")
        .cloned()
        .ok_or("未能从视频平台跳转中解析 tokenId")?;

    let envelope = client
        .get(format!("{VIDEO_API}/lti3/getAccessTokenByTokenId"))
        .query(&[("tokenId", token_id.as_str())])
        .send()
        .await
        .map_err(|e| format!("视频授权失败：{e}"))?
        .json::<ApiEnvelope<ExchangeData>>()
        .await
        .map_err(|e| format!("视频授权响应异常：{e}"))?;
    let error = api_error(&envelope, "视频授权失败");
    let data = envelope.data.ok_or(error)?;
    let canvas_course_id = [
        data.params.cour_id.as_str(),
        data.params.lti_course_id.as_str(),
    ]
    .iter()
    .find(|value| !value.is_empty())
    .map(|value| (*value).to_string())
    .ok_or("未能从视频平台解析课程 ID")?;
    Ok(VideoSession {
        token: data.token,
        canvas_course_id,
        course_id,
    })
}

#[tauri::command]
async fn open_login(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.stop_qr_login()?;
    state.replace_cookie_jar()?;
    let jar = state.cookie_jar()?;
    let controller = spawn_qr_login(app, jar);
    *state.qr_login.lock().map_err(|_| "会话状态不可用")? = Some(controller);
    Ok(())
}

#[tauri::command]
async fn refresh_qr_code(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let guard = state.qr_login.lock().map_err(|_| "会话状态不可用")?;
    let controller = guard
        .as_ref()
        .ok_or("请先开始扫码登录")?;
    controller.refresh();
    Ok(())
}

async fn cookie_header_for(jar: &Jar, origin: &str) -> Result<String, String> {
    let parsed = Url::parse(origin).map_err(|e| e.to_string())?;
    let cookie = jar
        .cookies(&parsed)
        .and_then(|value| value.to_str().ok().map(str::to_string))
        .ok_or_else(|| "没有找到登录 Cookie，请重新扫码".to_string())?;
    if cookie.is_empty() {
        return Err("没有找到登录 Cookie，请重新扫码".into());
    }
    Ok(cookie)
}

fn http_client(jar: Arc<Jar>) -> Result<Client, String> {
    client_with_jar(jar)
}

fn http_client_no_redirect(jar: Arc<Jar>) -> Result<Client, String> {
    Client::builder()
        .cookie_provider(jar)
        .user_agent("CanvasPocket/0.1")
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_courses(state: tauri::State<'_, AppState>) -> Result<Vec<Course>, String> {
    let jar = state.cookie_jar()?;
    let _ = cookie_header_for(&jar, CANVAS_ORIGIN).await?;
    let client = http_client(jar)?;
    let enrollment_states = ["active", "completed", "invited_or_pending"];
    let mut merged: HashMap<u64, Course> = HashMap::new();

    for enrollment_state in enrollment_states {
        let batch = fetch_courses_by_enrollment(&client, enrollment_state).await?;
        for mut course in batch {
            if course.name.trim().is_empty() {
                continue;
            }
            course.enrollment_state = Some(enrollment_state.to_string());
            merged
                .entry(course.id)
                .and_modify(|existing| {
                    if enrollment_rank(enrollment_state)
                        < enrollment_rank(existing.enrollment_state.as_deref().unwrap_or(""))
                    {
                        existing.enrollment_state = Some(enrollment_state.to_string());
                    }
                })
                .or_insert(course);
        }
    }

    let mut courses: Vec<Course> = merged.into_values().collect();
    courses.sort_by(|left, right| {
        course_sort_timestamp(right)
            .cmp(&course_sort_timestamp(left))
            .then_with(|| right.id.cmp(&left.id))
    });
    Ok(courses)
}

fn enrollment_rank(state: &str) -> u8 {
    match state {
        "active" => 0,
        "invited_or_pending" => 1,
        "completed" => 2,
        _ => 3,
    }
}

fn parse_canvas_time(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.timestamp())
        .or_else(|| {
            NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .ok()
                .and_then(|date| date.and_hms_opt(0, 0, 0))
                .map(|time| Utc.from_utc_datetime(&time).timestamp())
        })
}

fn course_sort_timestamp(course: &Course) -> i64 {
    [&course.end_at, &course.start_at, &course.created_at]
        .into_iter()
        .flatten()
        .find_map(|value| parse_canvas_time(value))
        .unwrap_or_else(|| course.id as i64)
}

async fn fetch_courses_by_enrollment(
    client: &Client,
    enrollment_state: &str,
) -> Result<Vec<Course>, String> {
    let mut page = 1u32;
    let mut courses = Vec::new();
    loop {
        let response = client
            .get(format!("{CANVAS_ORIGIN}/api/v1/courses"))
            .query(&[
                ("enrollment_state", enrollment_state),
                ("per_page", "100"),
                ("page", &page.to_string()),
            ])
            .send()
            .await
            .map_err(|e| format!("读取课程失败：{e}"))?;
        if !response.status().is_success() {
            return Err(format!("Canvas 返回 {}，请重新扫码", response.status()));
        }
        let body = response
            .text()
            .await
            .map_err(|e| format!("读取课程失败：{e}"))?;
        let values: Vec<Value> = serde_json::from_str(&body).map_err(|error| {
            format!(
                "课程数据格式异常：{error}；内容：{}",
                body.chars().take(240).collect::<String>()
            )
        })?;
        let batch: Vec<Course> = values.iter().filter_map(course_from_value).collect();
        let count = values.len();
        courses.extend(batch);
        if count < 100 {
            break;
        }
        page += 1;
    }
    Ok(courses)
}

fn json_u64(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(|entry| {
        entry
            .as_u64()
            .or_else(|| entry.as_i64().map(|number| number as u64))
            .or_else(|| entry.as_str()?.parse().ok())
    })
}

fn json_opt_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        let entry = value.get(*key)?;
        match entry {
            Value::Null => None,
            Value::String(text) if text.trim().is_empty() => None,
            Value::String(text) => Some(text.trim().to_string()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        }
    })
}

fn course_from_value(value: &Value) -> Option<Course> {
    let id = json_u64(value, "id")?;
    let course_code = json_string(value, &["course_code", "courseCode"]);
    let name = json_string(value, &["name"])
        .trim()
        .to_string()
        .or_else_if_empty(|| course_code.clone())
        .or_else_if_empty(|| format!("课程 {id}"));
    if name.trim().is_empty() {
        return None;
    }
    Some(Course {
        id,
        name,
        course_code,
        start_at: json_opt_string(value, &["start_at", "startAt"]),
        end_at: json_opt_string(value, &["end_at", "endAt"]),
        created_at: json_opt_string(value, &["created_at", "createdAt"]),
        workflow_state: json_opt_string(value, &["workflow_state", "workflowState"]),
        enrollment_state: None,
    })
}

fn json_string(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .unwrap_or_default()
        .to_string()
}

fn json_i64(value: &Value, key: &str) -> i64 {
    value
        .get(key)
        .and_then(|entry| entry.as_i64().or_else(|| entry.as_str()?.parse().ok()))
        .unwrap_or_default()
}

fn lessons_from_records(records: Vec<Value>) -> Vec<Lesson> {
    records
        .into_iter()
        .enumerate()
        .filter_map(|(index, value)| {
            let video_id = json_string(&value, &["videoId"]);
            if video_id.is_empty() {
                return None;
            }
            Some(Lesson {
                video_id,
                title: json_string(&value, &["courseName", "videoName", "title"])
                    .trim()
                    .to_string()
                    .or_else_if_empty(|| format!("第{:02}讲", index + 1)),
                begin_time: json_string(&value, &["courseBeginTime", "beginTime"]),
                end_time: json_string(&value, &["courseEndTime", "endTime"]),
                classroom: json_string(&value, &["classroomName", "classroom", "roomName"]),
                audit_status: json_i64(&value, "videAuditStatus"),
            })
        })
        .collect()
}

trait StringFallback {
    fn or_else_if_empty(self, fallback: impl FnOnce() -> String) -> String;
}

impl StringFallback for String {
    fn or_else_if_empty(self, fallback: impl FnOnce() -> String) -> String {
        if self.is_empty() { fallback() } else { self }
    }
}

fn nested_value<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

fn extract_video_records(payload: &Value) -> Option<Vec<Value>> {
    if let Some(array) = payload.as_array() {
        return Some(array.clone());
    }
    let paths: &[&[&str]] = &[
        &["data", "records"],
        &["data", "list"],
        &["data", "rows"],
        &["data", "items"],
        &["data", "page", "records"],
        &["data", "page", "list"],
        &["body", "list"],
        &["body"],
        &["data"],
    ];
    for path in paths {
        if let Some(Value::Array(records)) = nested_value(payload, path) {
            return Some(records.clone());
        }
    }
    None
}

fn course_id_candidates(course_id: u64, canvas_course_id: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    for value in [canvas_course_id.to_string(), course_id.to_string()] {
        if value.is_empty() || candidates.contains(&value) {
            continue;
        }
        candidates.push(value.clone());
        if value.chars().all(|ch| ch.is_ascii_digit()) {
            let trimmed = value.trim_start_matches('0').to_string();
            if !trimmed.is_empty() && !candidates.contains(&trimmed) {
                candidates.push(trimmed);
            }
        }
        let encoded = urlencoding::encode(&value).into_owned();
        if !encoded.is_empty() && !candidates.contains(&encoded) {
            candidates.push(encoded);
        }
    }
    candidates
}

async fn request_video_list(
    client: &Client,
    session: &VideoSession,
) -> Result<Vec<Value>, String> {
    let candidate_ids = course_id_candidates(session.course_id, &session.canvas_course_id);
    let mut last_summary = String::new();

    for candidate_id in candidate_ids {
        let bodies = [
            serde_json::json!({ "canvasCourseId": candidate_id }),
            serde_json::json!({ "canvasCourseId": candidate_id, "pageIndex": 1, "pageSize": 1000 }),
            serde_json::json!({ "courId": candidate_id }),
            serde_json::json!({ "courId": candidate_id, "pageIndex": 1, "pageSize": 1000 }),
            serde_json::json!({ "courseId": candidate_id }),
            serde_json::json!({ "ltiCourseId": candidate_id }),
        ];
        for body in bodies {
            let response = client
                .post(format!("{VIDEO_API}/directOnDemandPlay/findVodVideoList"))
                .header("token", &session.token)
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("读取讲次失败：{e}"))?;
            let payload: Value = response
                .json()
                .await
                .map_err(|e| format!("讲次响应异常：{e}"))?;
            if let Some(records) = extract_video_records(&payload) {
                return Ok(records);
            }
            last_summary = format!(
                "code={:?}, message={:?}, data_type={}",
                payload.get("code"),
                payload.get("message").or_else(|| payload.get("msg")),
                payload
                    .get("data")
                    .map(|value| match value {
                        Value::Null => "null",
                        Value::Object(_) => "object",
                        Value::Array(_) => "array",
                        Value::String(_) => "string",
                        _ => "other",
                    })
                    .unwrap_or("missing")
            );
        }
    }

    Err(format!(
        "视频列表接口未返回可识别的数据，尝试课程 ID：{}，最后一次返回：{last_summary}",
        course_id_candidates(session.course_id, &session.canvas_course_id).join(", ")
    ))
}

#[tauri::command]
async fn load_lessons(
    state: tauri::State<'_, AppState>,
    course_id: u64,
) -> Result<Vec<Lesson>, String> {
    let jar = state.cookie_jar()?;
    let video_session = authorize_video_session(jar, course_id).await?;
    let client = http_client(state.cookie_jar()?)?;
    let records = request_video_list(&client, &video_session).await?;
    *state.session.lock().map_err(|_| "会话状态不可用")? = Some(video_session);
    Ok(lessons_from_records(records))
}

fn video_api_message(payload: &Value) -> Option<String> {
    payload
        .get("message")
        .or_else(|| payload.get("msg"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn video_payload_detail(payload: &Value) -> Result<VideoDetail, String> {
    if payload.get("success").and_then(Value::as_bool) == Some(false) {
        return Err(video_api_message(payload).unwrap_or_else(|| "视频服务拒绝请求".into()));
    }
    if let Some(message) = video_api_message(payload) {
        if message.contains("解密") {
            return Err(message);
        }
    }
    if payload.get("data").and_then(Value::as_str).is_some() {
        return Err(video_api_message(payload).unwrap_or_else(|| "视频详情返回加密数据，请重新进入课程".into()));
    }
    for key in ["data", "body"] {
        let Some(node) = payload.get(key) else {
            continue;
        };
        if let Ok(detail) = serde_json::from_value::<VideoDetail>(node.clone()) {
            if !detail.video_play_response_vo_list.is_empty() {
                return Ok(detail);
            }
        }
    }
    Err(format!(
        "视频详情接口未返回可识别的数据：{}",
        video_api_message(payload).unwrap_or_else(|| {
            payload
                .as_object()
                .map(|obj| obj.keys().cloned().collect::<Vec<_>>().join(", "))
                .unwrap_or_else(|| payload.to_string())
        })
    ))
}

async fn post_video_form(
    client: &Client,
    session: &VideoSession,
    path: &str,
    fields: &[(&str, &str)],
) -> Result<Value, String> {
    let response = client
        .post(format!("{VIDEO_API}{path}"))
        .header("token", &session.token)
        .header(header::ACCEPT, "application/json, text/plain, */*")
        .form(fields)
        .send()
        .await
        .map_err(|e| format!("读取视频分轨失败：{e}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("读取视频分轨失败：{e}"))?;
    if !status.is_success() {
        return Err(format!(
            "视频服务返回 {}：{}",
            status,
            body.chars().take(240).collect::<String>()
        ));
    }
    serde_json::from_str(&body).map_err(|error| {
        format!(
            "视频分轨响应异常：{error}；内容：{}",
            body.chars().take(240).collect::<String>()
        )
    })
}

async fn fetch_video_detail(
    client: &Client,
    session: &VideoSession,
    lesson_id: &str,
) -> Result<VideoDetail, String> {
    let payload = post_video_form(
        client,
        session,
        "/directOnDemandPlay/getVodVideoInfos",
        &[
            ("playTypeHls", "true"),
            ("isAudit", "true"),
            ("id", lesson_id),
        ],
    )
    .await?;
    video_payload_detail(&payload)
}

fn track_name(code: i64) -> &'static str {
    match code {
        0 => "教师",
        1 => "学生1",
        2 => "学生2",
        3 => "PPT",
        4 => "合成",
        _ => "视频",
    }
}

fn safe_filename(value: &str) -> String {
    let invalid = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    let result = value
        .chars()
        .map(|ch| {
            if invalid.contains(&ch) || ch.is_control() {
                '_'
            } else {
                ch
            }
        })
        .collect::<String>();
    let trimmed = result.trim().trim_end_matches(['.', ' ']).trim();
    if trimmed.is_empty() {
        "课程录像".into()
    } else {
        trimmed.chars().take(120).collect()
    }
}

fn emit_progress(app: &tauri::AppHandle, progress: DownloadProgress) {
    if let Some(state) = app.try_state::<AppState>() {
        state.record_progress(&progress);
    }
    let _ = app.emit("download-progress", progress);
}

struct DownloadTaskContext {
    task_id: String,
    lesson_title: String,
    signal: String,
    control: TaskControl,
}

async fn download_track(
    app: &tauri::AppHandle,
    client: &Client,
    request: &DownloadRequest,
    track: &VideoTrack,
    directory: &Path,
    context: &DownloadTaskContext,
) -> Result<(), String> {
    let download_url = track_download_url(track);
    if download_url.is_empty() {
        return Err(format!("{} 分轨没有可用的下载地址", context.signal));
    }

    let date = request.begin_time.get(..10).unwrap_or("");
    let stem = safe_filename(&format!(
        "{}_{}_{}",
        request.lesson_title, date, context.signal
    ));
    let extension = download_url
        .split('?')
        .next()
        .and_then(|url| url.rsplit('.').next())
        .filter(|ext| ext.len() <= 5)
        .unwrap_or("mp4");
    let file_name = format!("{stem}.{extension}");
    let destination = directory.join(&file_name);
    let partial = directory.join(format!("{file_name}.part"));
    if destination.exists() {
        emit_progress(
            app,
            DownloadProgress {
                task_id: context.task_id.clone(),
                lesson_id: request.lesson_id.clone(),
                lesson_title: context.lesson_title.clone(),
                signal: context.signal.clone(),
                file_name,
                stage: "completed",
                downloaded: destination.metadata().map(|m| m.len()).unwrap_or(0),
                total: destination.metadata().map(|m| m.len()).ok(),
                message: "文件已存在".into(),
            },
        );
        return Ok(());
    }

    let existing = fs::metadata(&partial)
        .await
        .map(|meta| meta.len())
        .unwrap_or(0);
    let mut builder = client
        .get(download_url)
        .header(header::REFERER, VIDEO_REFERER);
    if existing > 0 {
        builder = builder.header(header::RANGE, format!("bytes={existing}-"));
    }
    let response = builder
        .send()
        .await
        .map_err(|e| format!("下载请求失败：{e}"))?;
    if context.control.is_cancelled() {
        return Err("下载已取消".into());
    }
    if !response.status().is_success() && response.status() != StatusCode::PARTIAL_CONTENT {
        return Err(format!("视频服务返回 {}", response.status()));
    }

    let resumed = response.status() == StatusCode::PARTIAL_CONTENT && existing > 0;
    let base = if resumed { existing } else { 0 };
    let total = response.content_length().map(|length| length + base);
    let mut options = fs::OpenOptions::new();
    options.create(true).write(true);
    if resumed {
        options.append(true);
    } else {
        options.truncate(true);
    }
    let mut file = options
        .open(&partial)
        .await
        .map_err(|e| format!("无法创建文件：{e}"))?;
    let mut downloaded = base;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if context.control.is_cancelled() {
            file.flush().await.ok();
            emit_progress(
                app,
                DownloadProgress {
                    task_id: context.task_id.clone(),
                    lesson_id: request.lesson_id.clone(),
                    lesson_title: context.lesson_title.clone(),
                    signal: context.signal.clone(),
                    file_name: file_name.clone(),
                    stage: "cancelled",
                    downloaded,
                    total,
                    message: "下载已取消".into(),
                },
            );
            return Err("下载已取消".into());
        }
        if context.control.is_paused() {
            file.flush().await.ok();
            emit_progress(
                app,
                DownloadProgress {
                    task_id: context.task_id.clone(),
                    lesson_id: request.lesson_id.clone(),
                    lesson_title: context.lesson_title.clone(),
                    signal: context.signal.clone(),
                    file_name: file_name.clone(),
                    stage: "paused",
                    downloaded,
                    total,
                    message: "已暂停，可继续下载".into(),
                },
            );
            return Ok(());
        }
        let bytes = chunk.map_err(|e| format!("下载中断：{e}"))?;
        file.write_all(&bytes)
            .await
            .map_err(|e| format!("写入失败：{e}"))?;
        downloaded += bytes.len() as u64;
        emit_progress(
            app,
            DownloadProgress {
                task_id: context.task_id.clone(),
                lesson_id: request.lesson_id.clone(),
                lesson_title: context.lesson_title.clone(),
                signal: context.signal.clone(),
                file_name: file_name.clone(),
                stage: "downloading",
                downloaded,
                total,
                message: "正在下载".into(),
            },
        );
    }
    file.flush().await.map_err(|e| e.to_string())?;
    fs::rename(&partial, &destination)
        .await
        .map_err(|e| format!("完成文件失败：{e}"))?;
    emit_progress(
        app,
        DownloadProgress {
            task_id: context.task_id.clone(),
            lesson_id: request.lesson_id.clone(),
            lesson_title: context.lesson_title.clone(),
            signal: context.signal.clone(),
            file_name,
            stage: "completed",
            downloaded,
            total: Some(downloaded),
            message: "视频下载完成".into(),
        },
    );
    Ok(())
}

async fn run_download_task(app: tauri::AppHandle, jar: Arc<Jar>, task_id: String) {
    let (meta, control, _, running) = match app.state::<AppState>().get_download(&task_id) {
        Ok(parts) => parts,
        Err(message) => {
            emit_progress(
                &app,
                DownloadProgress {
                    task_id: task_id.clone(),
                    lesson_id: String::new(),
                    lesson_title: String::new(),
                    signal: String::new(),
                    file_name: String::new(),
                    stage: "failed",
                    downloaded: 0,
                    total: None,
                    message,
                },
            );
            return;
        }
    };
    if running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    emit_progress(
        &app,
        DownloadProgress {
            task_id: task_id.clone(),
            lesson_id: meta.lesson_id.clone(),
            lesson_title: meta.lesson_title.clone(),
            signal: meta.signal.clone(),
            file_name: String::new(),
            stage: "downloading",
            downloaded: 0,
            total: None,
            message: "正在获取下载地址".into(),
        },
    );

    let slots = match app.state::<AppState>().download_slots() {
        Ok(slots) => slots,
        Err(message) => {
            running.store(false, Ordering::Release);
            emit_progress(
                &app,
                DownloadProgress {
                    task_id: task_id.clone(),
                    lesson_id: meta.lesson_id.clone(),
                    lesson_title: meta.lesson_title.clone(),
                    signal: meta.signal.clone(),
                    file_name: String::new(),
                    stage: "failed",
                    downloaded: 0,
                    total: None,
                    message,
                },
            );
            return;
        }
    };
    let _permit = match slots.acquire().await {
        Ok(permit) => permit,
        Err(error) => {
            running.store(false, Ordering::Release);
            emit_progress(
                &app,
                DownloadProgress {
                    task_id: task_id.clone(),
                    lesson_id: meta.lesson_id.clone(),
                    lesson_title: meta.lesson_title.clone(),
                    signal: meta.signal.clone(),
                    file_name: String::new(),
                    stage: "failed",
                    downloaded: 0,
                    total: None,
                    message: error.to_string(),
                },
            );
            return;
        }
    };

    let result: Result<(), String> = async {
        let client = http_client(jar)?;
        let detail = fetch_video_detail(&client, &meta.session, &meta.lesson_id).await?;
        let track = detail
            .video_play_response_vo_list
            .iter()
            .find(|track| track.cdvi_view_num == meta.cdvi_view_num)
            .ok_or_else(|| format!("讲次中找不到 {} 分轨", meta.signal))?;
        let request = DownloadRequest {
            lesson_id: meta.lesson_id.clone(),
            lesson_title: meta.lesson_title.clone(),
            begin_time: meta.begin_time.clone(),
            signals: vec![meta.signal.clone()],
            output_dir: Some(
                meta.output_dir
                    .to_string_lossy()
                    .into_owned(),
            ),
            course_name: None,
        };
        let context = DownloadTaskContext {
            task_id: task_id.clone(),
            lesson_title: meta.lesson_title.clone(),
            signal: meta.signal.clone(),
            control,
        };
        download_track(
            &app,
            &client,
            &request,
            track,
            &meta.output_dir,
            &context,
        )
        .await
    }
    .await;

    running.store(false, Ordering::Release);

    if let Err(message) = result {
        if app
            .state::<AppState>()
            .get_download(&task_id)
            .map(|(_, control, _, _)| control.is_cancelled())
            .unwrap_or(false)
        {
            return;
        }
        let stage = if message == "下载已取消" {
            "cancelled"
        } else {
            "failed"
        };
        emit_progress(
            &app,
            DownloadProgress {
                task_id,
                lesson_id: meta.lesson_id,
                lesson_title: meta.lesson_title,
                signal: meta.signal,
                file_name: String::new(),
                stage,
                downloaded: 0,
                total: None,
                message,
            },
        );
    }
}

fn default_download_root() -> PathBuf {
    dirs::download_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Canvas Downloads")
}

fn course_download_dir(base: &Path, course_name: Option<&str>, course_id: u64) -> PathBuf {
    let label = course_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(safe_filename)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("course_{course_id}"));
    base.join(label)
}

#[tauri::command]
fn default_download_dir() -> String {
    default_download_root().to_string_lossy().into_owned()
}

#[tauri::command]
async fn start_download(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    request: DownloadRequest,
) -> Result<Vec<String>, String> {
    let session = state
        .session
        .lock()
        .map_err(|_| "视频会话不可用")?
        .clone()
        .ok_or_else(|| "请先选择课程并读取讲次".to_string())?;
    let jar = state.cookie_jar()?;
    let client = http_client(jar.clone())?;
    let base_dir = request
        .output_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(default_download_root);
    let directory = course_download_dir(
        &base_dir,
        request.course_name.as_deref(),
        session.course_id,
    );
    fs::create_dir_all(&directory)
        .await
        .map_err(|e| format!("无法创建保存目录：{e}"))?;
    let detail = fetch_video_detail(&client, &session, &request.lesson_id).await?;
    let wanted: HashSet<&str> = request.signals.iter().map(String::as_str).collect();
    let matched: Vec<_> = detail
        .video_play_response_vo_list
        .iter()
        .filter(|track| wanted.contains(track_name(track.cdvi_view_num)))
        .collect();
    if matched.is_empty() {
        let available: Vec<_> = detail
            .video_play_response_vo_list
            .iter()
            .map(|track| track_name(track.cdvi_view_num))
            .collect();
        return Err(format!(
            "当前讲次没有所选分轨，可用分轨：{}",
            if available.is_empty() {
                "无".into()
            } else {
                available.join("、")
            }
        ));
    }

    let mut task_ids = Vec::new();
    for track in matched {
        let task_id = uuid::Uuid::new_v4().to_string();
        let signal = track_name(track.cdvi_view_num).to_string();
        let control = TaskControl::new();
        let progress = DownloadProgress {
            task_id: task_id.clone(),
            lesson_id: request.lesson_id.clone(),
            lesson_title: request.lesson_title.clone(),
            signal: signal.clone(),
            file_name: String::new(),
            stage: "queued",
            downloaded: 0,
            total: None,
            message: "等待下载位置".into(),
        };
        state.register_download(
            task_id.clone(),
            ActiveDownload {
                meta: DownloadTaskMeta {
                    session: session.clone(),
                    lesson_id: request.lesson_id.clone(),
                    lesson_title: request.lesson_title.clone(),
                    begin_time: request.begin_time.clone(),
                    output_dir: directory.clone(),
                    cdvi_view_num: track.cdvi_view_num,
                    signal,
                },
                control: control.clone(),
                progress: progress.clone(),
                running: Arc::new(AtomicBool::new(false)),
            },
        )?;
        emit_progress(&app, progress);
        task_ids.push(task_id.clone());
        let app_spawn = app.clone();
        let jar_spawn = jar.clone();
        tauri::async_runtime::spawn(async move {
            run_download_task(app_spawn, jar_spawn, task_id).await;
        });
    }
    Ok(task_ids)
}

#[tauri::command]
fn list_download_tasks(state: tauri::State<'_, AppState>) -> Result<Vec<DownloadProgress>, String> {
    let guard = state.downloads.lock().map_err(|_| "下载任务状态不可用")?;
    let mut tasks: Vec<_> = guard.values().map(|task| task.progress.clone()).collect();
    tasks.sort_by(|left, right| right.task_id.cmp(&left.task_id));
    Ok(tasks)
}

#[tauri::command]
fn pause_download(state: tauri::State<'_, AppState>, task_id: String) -> Result<(), String> {
    let (_, control, progress, _) = state.get_download(&task_id)?;
    if progress.stage == "completed" || progress.stage == "cancelled" {
        return Err("该任务已结束，无法暂停".into());
    }
    control.pause();
    Ok(())
}

#[tauri::command]
async fn resume_download(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    task_id: String,
) -> Result<(), String> {
    let (meta, control, progress, running) = state.get_download(&task_id)?;
    if progress.stage == "completed" {
        return Err("该任务已完成".into());
    }
    if running.load(Ordering::Relaxed) {
        control.resume();
        return Ok(());
    }
    control.resume();
    let jar = state.cookie_jar()?;
    emit_progress(
        &app,
        DownloadProgress {
            task_id: task_id.clone(),
            lesson_id: meta.lesson_id.clone(),
            lesson_title: meta.lesson_title.clone(),
            signal: meta.signal.clone(),
            file_name: progress.file_name,
            stage: "queued",
            downloaded: progress.downloaded,
            total: progress.total,
            message: "准备继续下载".into(),
        },
    );
    tauri::async_runtime::spawn(async move {
        run_download_task(app, jar, task_id).await;
    });
    Ok(())
}

#[tauri::command]
fn get_download_concurrency(state: tauri::State<'_, AppState>) -> Result<usize, String> {
    state.download_concurrency_limit()
}

#[tauri::command]
fn set_download_concurrency(
    state: tauri::State<'_, AppState>,
    concurrency: usize,
) -> Result<usize, String> {
    state.set_download_concurrency(concurrency)
}

#[tauri::command]
fn cancel_download(state: tauri::State<'_, AppState>, task_id: String) -> Result<(), String> {
    let (_, control, _, _) = state.get_download(&task_id)?;
    control.cancel();
    Ok(())
}

#[tauri::command]
async fn logout(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.stop_qr_login()?;
    state.replace_cookie_jar()?;
    *state.session.lock().map_err(|_| "会话状态不可用")? = None;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::new())
        .setup(|app| {
            if let Some(main) = app.get_webview_window("main") {
                let handle = app.handle().clone();
                main.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { .. } = event {
                        handle.exit(0);
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            open_login,
            refresh_qr_code,
            list_courses,
            load_lessons,
            default_download_dir,
            start_download,
            list_download_tasks,
            pause_download,
            resume_download,
            cancel_download,
            get_download_concurrency,
            set_download_concurrency,
            logout
        ])
        .run(tauri::generate_context!())
        .expect("error while running Canvas Pocket");
}
