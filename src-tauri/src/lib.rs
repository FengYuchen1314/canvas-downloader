use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use futures_util::StreamExt;
use reqwest::{Client, StatusCode, header, multipart};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tokio::{fs, io::AsyncWriteExt, sync::Semaphore};
use url::Url;

const CANVAS_ORIGIN: &str = "https://oc.sjtu.edu.cn";
const VIDEO_ORIGIN: &str = "https://v.sjtu.edu.cn";
const VIDEO_API: &str = "https://v.sjtu.edu.cn/jy-application-canvas-sjtu";

const LOGIN_SCRIPT: &str = r#"
(() => {
  if (window.top !== window) return;
  let clickedJaccount = false;
  let observer;
  const text = (node) => (node?.innerText || node?.textContent || node?.value || '').replace(/\s+/g, ' ').trim();
  const hide = (node) => {
    if (!node) return;
    node.style.setProperty('display', 'none', 'important');
    node.setAttribute('aria-hidden', 'true');
  };
  const clickable = () => [...document.querySelectorAll('a,button,input[type="button"],input[type="submit"]')];
  const tick = () => {
    const host = location.hostname;
    if (host === 'oc.sjtu.edu.cn') {
      const isLogin = location.pathname.includes('/login') || /登录/.test(document.title);
      if (isLogin && !clickedJaccount) {
        const target = clickable().find((node) => /jaccount|统一身份|校内用户/i.test(text(node)));
        if (target) { clickedJaccount = true; target.click(); }
      }
    }
    if (host === 'jaccount.sjtu.edu.cn') {
      const controls = clickable();
      controls.forEach((node) => {
        const href = node.getAttribute('href') || '';
        if (
          /账号密码|短信验证|密码登录|短信登录|交我办快速登录/.test(text(node)) ||
          href.startsWith('jaccount://') ||
          href === 'https://jaccount.sjtu.edu.cn/jaccount/#'
        ) hide(node);
      });
      document.querySelectorAll('input[type="password"], input[name*="password" i], input[name*="username" i], input[autocomplete="username"]').forEach((node) => {
        hide(node);
        hide(node.parentElement);
      });
    }
  };
  const start = () => {
    document.addEventListener('click', (event) => {
      const target = event.target?.closest?.('a,button');
      if (!target) return;
      const href = target.getAttribute('href') || '';
      if (href.startsWith('jaccount://') || /交我办快速登录/.test(text(target))) {
        event.preventDefault();
        event.stopImmediatePropagation();
      }
    }, true);
    tick();
    if (document.documentElement && !observer) {
      observer = new MutationObserver(tick);
      observer.observe(document.documentElement, { childList: true, subtree: true });
    }
  };
  if (document.documentElement) start();
  else document.addEventListener('DOMContentLoaded', start, { once: true });
  setInterval(tick, 700);
})();
"#;

struct AppState {
    session: Mutex<Option<VideoSession>>,
    download_slots: Arc<Semaphore>,
}

impl AppState {
    fn new() -> Self {
        Self {
            session: Mutex::new(None),
            download_slots: Arc::new(Semaphore::new(2)),
        }
    }
}

#[derive(Clone, Debug)]
struct VideoSession {
    token: String,
    params: AccessParams,
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
struct AuthStatus {
    phase: &'static str,
    message: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct Course {
    id: u64,
    name: String,
    #[serde(default)]
    course_code: String,
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
    include_subtitles: bool,
    output_dir: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DownloadProgress {
    task_id: String,
    lesson_id: String,
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

#[derive(Deserialize)]
struct LessonPage {
    #[serde(default)]
    records: Vec<Value>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct VideoDetail {
    #[serde(default)]
    cour_id: String,
    #[serde(default)]
    video_play_response_vo_list: Vec<VideoTrack>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct VideoTrack {
    id: String,
    #[serde(default)]
    cdvi_view_num: i64,
    #[serde(default)]
    rtmp_url_hdv: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubtitleData {
    #[serde(default)]
    after_assembly_list: Vec<SubtitleCue>,
}

#[derive(Deserialize)]
struct SubtitleCue {
    #[serde(default)]
    bg: i64,
    #[serde(default)]
    ed: i64,
    #[serde(default)]
    res: String,
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

fn allowed_navigation(url: &Url) -> bool {
    matches!(
        url.host_str(),
        Some("oc.sjtu.edu.cn") | Some("jaccount.sjtu.edu.cn") | Some("v.sjtu.edu.cn")
    )
}

fn classify_auth_url(url: &Url) -> AuthStatus {
    match url.host_str() {
        Some("jaccount.sjtu.edu.cn") => AuthStatus {
            phase: "qr-login",
            message: "请使用交我办扫码".into(),
        },
        Some("oc.sjtu.edu.cn") if !url.path().contains("/login") => AuthStatus {
            phase: "authorized",
            message: "Canvas 已连接".into(),
        },
        Some("oc.sjtu.edu.cn") => AuthStatus {
            phase: "canvas-login",
            message: "正在进入 jAccount".into(),
        },
        Some("v.sjtu.edu.cn") => AuthStatus {
            phase: "authorized",
            message: "课堂视频已授权".into(),
        },
        _ => AuthStatus {
            phase: "error",
            message: "登录跳转到了未允许的地址".into(),
        },
    }
}

#[tauri::command]
async fn open_login(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("login") {
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
        if let Ok(url) = window.url() {
            let _ = app.emit("auth-status", classify_auth_url(&url));
        }
        return Ok(());
    }

    let url = Url::parse(CANVAS_ORIGIN).map_err(|e| e.to_string())?;
    let events = app.clone();
    let login_window = WebviewWindowBuilder::new(&app, "login", WebviewUrl::External(url))
        .title("交我办扫码登录")
        .inner_size(520.0, 760.0)
        .min_inner_size(420.0, 600.0)
        .resizable(true)
        .initialization_script(LOGIN_SCRIPT)
        .on_navigation(allowed_navigation)
        .on_page_load(move |_window, payload| {
            let status = classify_auth_url(payload.url());
            let _ = events.emit("auth-status", status);
        })
        .build()
        .map_err(|e| e.to_string())?;
    let window_to_hide = login_window.clone();
    let close_events = app.clone();
    login_window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = window_to_hide.hide();
            let status = window_to_hide
                .url()
                .map(|url| classify_auth_url(&url))
                .unwrap_or(AuthStatus {
                    phase: "canvas-login",
                    message: "登录窗口已隐藏，可随时重新打开".into(),
                });
            let _ = close_events.emit("auth-status", status);
        }
    });
    Ok(())
}

async fn cookie_header_for(window: WebviewWindow, url: &str) -> Result<String, String> {
    let parsed = Url::parse(url).map_err(|e| e.to_string())?;
    let cookies = tokio::task::spawn_blocking(move || window.cookies_for_url(parsed))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    if cookies.is_empty() {
        return Err("没有找到登录 Cookie，请重新扫码".into());
    }
    Ok(cookies
        .iter()
        .map(|cookie| format!("{}={}", cookie.name(), cookie.value()))
        .collect::<Vec<_>>()
        .join("; "))
}

fn http_client() -> Result<Client, String> {
    Client::builder()
        .user_agent("CanvasPocket/0.1")
        .redirect(reqwest::redirect::Policy::limited(8))
        .build()
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_courses(app: tauri::AppHandle) -> Result<Vec<Course>, String> {
    let window = app
        .get_webview_window("login")
        .ok_or_else(|| "请先打开扫码登录窗口".to_string())?;
    let cookie = cookie_header_for(window, CANVAS_ORIGIN).await?;
    let response = http_client()?
        .get(format!("{CANVAS_ORIGIN}/api/v1/courses"))
        .query(&[("enrollment_state", "active"), ("per_page", "100")])
        .header(header::COOKIE, cookie)
        .send()
        .await
        .map_err(|e| format!("读取课程失败：{e}"))?;
    if !response.status().is_success() {
        return Err(format!("Canvas 返回 {}，请重新扫码", response.status()));
    }
    let courses = response
        .json::<Vec<Course>>()
        .await
        .map_err(|e| format!("课程数据格式异常：{e}"))?;
    Ok(courses
        .into_iter()
        .filter(|course| !course.name.trim().is_empty())
        .collect())
}

fn token_from_video_url(url: &Url) -> Option<String> {
    if url.host_str() != Some("v.sjtu.edu.cn") {
        return None;
    }
    let fragment = url.fragment()?;
    let query = fragment.split_once('?')?.1;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "tokenId")
        .map(|(_, value)| value.into_owned())
}

async fn video_cookie(app: &tauri::AppHandle) -> Option<String> {
    let window = app.get_webview_window("login")?;
    cookie_header_for(window, VIDEO_ORIGIN).await.ok()
}

async fn exchange_video_token(
    app: &tauri::AppHandle,
    token_id: &str,
) -> Result<VideoSession, String> {
    let mut request = http_client()?
        .get(format!("{VIDEO_API}/lti3/getAccessTokenByTokenId"))
        .query(&[("tokenId", token_id)]);
    if let Some(cookie) = video_cookie(app).await {
        request = request.header(header::COOKIE, cookie);
    }
    let envelope = request
        .send()
        .await
        .map_err(|e| format!("视频授权失败：{e}"))?
        .json::<ApiEnvelope<ExchangeData>>()
        .await
        .map_err(|e| format!("视频授权响应异常：{e}"))?;
    let error = api_error(&envelope, "视频授权失败");
    let data = envelope.data.ok_or(error)?;
    Ok(VideoSession {
        token: data.token,
        params: data.params,
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

#[tauri::command]
async fn load_lessons(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    course_id: u64,
) -> Result<Vec<Lesson>, String> {
    let window = app
        .get_webview_window("login")
        .ok_or_else(|| "请先完成扫码登录".to_string())?;
    let lti_url = Url::parse(&format!(
        "{CANVAS_ORIGIN}/courses/{course_id}/external_tools/8329?display=borderless"
    ))
    .map_err(|e| e.to_string())?;
    window.navigate(lti_url).map_err(|e| e.to_string())?;

    let started = Instant::now();
    let token_id = loop {
        if started.elapsed() > Duration::from_secs(35) {
            return Err("课堂视频授权超时，请重新选择课程".into());
        }
        let current = window.url().map_err(|e| e.to_string())?;
        if let Some(token) = token_from_video_url(&current) {
            break token;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };

    let video_session = exchange_video_token(&app, &token_id).await?;
    let envelope = http_client()?
        .post(format!("{VIDEO_API}/directOnDemandPlay/findVodVideoList"))
        .header("token", &video_session.token)
        .json(&serde_json::json!({ "canvasCourseId": video_session.params.cour_id }))
        .send()
        .await
        .map_err(|e| format!("读取讲次失败：{e}"))?
        .json::<ApiEnvelope<LessonPage>>()
        .await
        .map_err(|e| format!("讲次响应异常：{e}"))?;
    let error = api_error(&envelope, "读取讲次失败");
    let page = envelope.data.ok_or(error)?;
    *state.session.lock().map_err(|_| "会话状态不可用")? = Some(video_session);
    Ok(lessons_from_records(page.records))
}

async fn fetch_video_detail(
    session: &VideoSession,
    lesson_id: &str,
) -> Result<VideoDetail, String> {
    let form = multipart::Form::new()
        .text("playTypeHls", "true")
        .text("isAudit", "true")
        .text("id", lesson_id.to_string());
    let envelope = http_client()?
        .post(format!("{VIDEO_API}/directOnDemandPlay/getVodVideoInfos"))
        .header("token", &session.token)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("读取视频分轨失败：{e}"))?
        .json::<ApiEnvelope<VideoDetail>>()
        .await
        .map_err(|e| format!("视频分轨响应异常：{e}"))?;
    let error = api_error(&envelope, "读取视频分轨失败");
    envelope.data.ok_or(error)
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
    let _ = app.emit("download-progress", progress);
}

async fn download_track(
    app: &tauri::AppHandle,
    client: &Client,
    session: &VideoSession,
    request: &DownloadRequest,
    track: &VideoTrack,
    directory: &Path,
    task_id: &str,
) -> Result<(), String> {
    let signal = track_name(track.cdvi_view_num);
    let date = request.begin_time.get(..10).unwrap_or("");
    let stem = safe_filename(&format!("{}_{}_{}", request.lesson_title, date, signal));
    let file_name = format!("{stem}.mp4");
    let destination = directory.join(&file_name);
    let partial = directory.join(format!("{file_name}.part"));
    if destination.exists() {
        emit_progress(
            app,
            DownloadProgress {
                task_id: task_id.into(),
                lesson_id: request.lesson_id.clone(),
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
    let encoded = BASE64.encode(track.id.as_bytes());
    let official_url = format!("{VIDEO_API}/directOnDemandPlay/downloadVideo?id={encoded}");
    let mut builder = client.get(&official_url).header("token", &session.token);
    if existing > 0 {
        builder = builder.header(header::RANGE, format!("bytes={existing}-"));
    }
    let mut response = builder
        .send()
        .await
        .map_err(|e| format!("下载请求失败：{e}"))?;
    if !response.status().is_success() {
        response = client
            .get(&track.rtmp_url_hdv)
            .send()
            .await
            .map_err(|e| format!("备用下载请求失败：{e}"))?;
    }
    if !response.status().is_success() {
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
        let bytes = chunk.map_err(|e| format!("下载中断：{e}"))?;
        file.write_all(&bytes)
            .await
            .map_err(|e| format!("写入失败：{e}"))?;
        downloaded += bytes.len() as u64;
        emit_progress(
            app,
            DownloadProgress {
                task_id: task_id.into(),
                lesson_id: request.lesson_id.clone(),
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
            task_id: task_id.into(),
            lesson_id: request.lesson_id.clone(),
            file_name,
            stage: "completed",
            downloaded,
            total: Some(downloaded),
            message: "视频下载完成".into(),
        },
    );
    Ok(())
}

fn srt_time(milliseconds: i64) -> String {
    let value = milliseconds.max(0);
    let hours = value / 3_600_000;
    let minutes = (value / 60_000) % 60;
    let seconds = (value / 1_000) % 60;
    let millis = value % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02},{millis:03}")
}

async fn download_subtitles(
    app: &tauri::AppHandle,
    client: &Client,
    session: &VideoSession,
    request: &DownloadRequest,
    detail: &VideoDetail,
    directory: &Path,
    task_id: &str,
) -> Result<(), String> {
    let envelope = client
        .post(format!("{VIDEO_API}/transfer/translate/detail"))
        .header("token", &session.token)
        .json(&serde_json::json!({ "courseId": detail.cour_id, "platform": 1 }))
        .send()
        .await
        .map_err(|e| format!("读取字幕失败：{e}"))?
        .json::<ApiEnvelope<SubtitleData>>()
        .await
        .map_err(|e| format!("字幕响应异常：{e}"))?;
    let Some(data) = envelope.data else {
        return Ok(());
    };
    if data.after_assembly_list.is_empty() {
        return Ok(());
    }
    let date = request.begin_time.get(..10).unwrap_or("");
    let file_name = format!(
        "{}.srt",
        safe_filename(&format!("{}_{}_AI字幕", request.lesson_title, date))
    );
    emit_progress(
        app,
        DownloadProgress {
            task_id: task_id.into(),
            lesson_id: request.lesson_id.clone(),
            file_name: file_name.clone(),
            stage: "writing-subtitles",
            downloaded: 0,
            total: None,
            message: "正在写入字幕".into(),
        },
    );
    let mut output = String::new();
    for (index, cue) in data.after_assembly_list.iter().enumerate() {
        let text = cue.res.trim();
        if text.is_empty() {
            continue;
        }
        output.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            index + 1,
            srt_time(cue.bg),
            srt_time(cue.ed),
            text
        ));
    }
    fs::write(directory.join(&file_name), output)
        .await
        .map_err(|e| format!("字幕写入失败：{e}"))?;
    emit_progress(
        app,
        DownloadProgress {
            task_id: task_id.into(),
            lesson_id: request.lesson_id.clone(),
            file_name,
            stage: "completed",
            downloaded: 0,
            total: None,
            message: "字幕已保存".into(),
        },
    );
    Ok(())
}

#[tauri::command]
fn default_download_dir() -> String {
    dirs::download_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Canvas Pocket")
        .to_string_lossy()
        .into_owned()
}

#[tauri::command]
async fn start_download(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    request: DownloadRequest,
) -> Result<String, String> {
    let session = state
        .session
        .lock()
        .map_err(|_| "视频会话不可用")?
        .clone()
        .ok_or_else(|| "请先选择课程并读取讲次".to_string())?;
    let slots = state.download_slots.clone();
    let task_id = uuid::Uuid::new_v4().to_string();
    let task_return = task_id.clone();
    let task_app = app.clone();
    tauri::async_runtime::spawn(async move {
        emit_progress(
            &task_app,
            DownloadProgress {
                task_id: task_id.clone(),
                lesson_id: request.lesson_id.clone(),
                file_name: request.lesson_title.clone(),
                stage: "queued",
                downloaded: 0,
                total: None,
                message: "等待下载位置".into(),
            },
        );
        let _permit = match slots.acquire().await {
            Ok(permit) => permit,
            Err(error) => {
                emit_progress(
                    &task_app,
                    DownloadProgress {
                        task_id: task_id.clone(),
                        lesson_id: request.lesson_id.clone(),
                        file_name: request.lesson_title.clone(),
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
            let directory = request
                .output_dir
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    dirs::download_dir()
                        .unwrap_or_else(|| PathBuf::from("."))
                        .join("Canvas Pocket")
                });
            fs::create_dir_all(&directory)
                .await
                .map_err(|e| format!("无法创建保存目录：{e}"))?;
            let client = http_client()?;
            let detail = fetch_video_detail(&session, &request.lesson_id).await?;
            let wanted: HashSet<&str> = request.signals.iter().map(String::as_str).collect();
            for track in detail
                .video_play_response_vo_list
                .iter()
                .filter(|track| wanted.contains(track_name(track.cdvi_view_num)))
            {
                download_track(
                    &task_app, &client, &session, &request, track, &directory, &task_id,
                )
                .await?;
            }
            if request.include_subtitles {
                download_subtitles(
                    &task_app, &client, &session, &request, &detail, &directory, &task_id,
                )
                .await?;
            }
            Ok(())
        }
        .await;
        if let Err(message) = result {
            emit_progress(
                &task_app,
                DownloadProgress {
                    task_id: task_id.clone(),
                    lesson_id: request.lesson_id.clone(),
                    file_name: request.lesson_title.clone(),
                    stage: "failed",
                    downloaded: 0,
                    total: None,
                    message,
                },
            );
        }
    });
    Ok(task_return)
}

#[tauri::command]
async fn logout(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("login") {
        let clear_window = window.clone();
        tokio::task::spawn_blocking(move || clear_window.clear_all_browsing_data())
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        window
            .navigate(Url::parse(CANVAS_ORIGIN).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        window.hide().map_err(|e| e.to_string())?;
    }
    *state.session.lock().map_err(|_| "会话状态不可用")? = None;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
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
            list_courses,
            load_lessons,
            default_download_dir,
            start_download,
            logout
        ])
        .run(tauri::generate_context!())
        .expect("error while running Canvas Pocket");
}
