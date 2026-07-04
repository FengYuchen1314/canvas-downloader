use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use reqwest::{Client, cookie::Jar, header};
use scraper::{Html, Selector};
use serde_json::Value;
use tauri::{AppHandle, Emitter};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use url::Url;

use crate::AuthStatus;

const LOGIN_INIT_URL: &str = "https://courses.sjtu.edu.cn/app/oauth/2.0/login?login_type=outer";
const CANVAS_LOGIN_URL: &str = "https://oc.sjtu.edu.cn/login/openid_connect";
const JACCOUNT_QR_URL: &str = "https://jaccount.sjtu.edu.cn/jaccount/qrcode";
const JACCOUNT_EXPRESS_URL: &str = "https://jaccount.sjtu.edu.cn/jaccount/expresslogin";
const JACCOUNT_WS_ORIGIN: &str = "https://jaccount.sjtu.edu.cn";

pub struct QrLoginController {
    refresh_tx: mpsc::Sender<()>,
    stop_tx: Option<oneshot::Sender<()>>,
}

impl QrLoginController {
    pub fn refresh(&self) {
        let _ = self.refresh_tx.try_send(());
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
    }
}

pub fn client_with_jar(jar: Arc<Jar>) -> Result<Client, String> {
    Client::builder()
        .cookie_provider(jar)
        .user_agent("CanvasPocket/0.1")
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| e.to_string())
}

fn extract_uuid(html: &str) -> Result<String, String> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("a#firefox_link").map_err(|e| e.to_string())?;
    let href = document
        .select(&selector)
        .next()
        .and_then(|node| node.value().attr("href"))
        .ok_or("未找到 jAccount 登录入口，页面结构可能已变化")?;
    href.rsplit('=')
        .next()
        .and_then(|tail| tail.split('&').next())
        .filter(|uuid| !uuid.is_empty())
        .map(str::to_string)
        .ok_or("无法从登录页解析 uuid")
}

async fn fetch_login_page(client: &Client) -> Result<(String, String), String> {
    let response = client
        .get(LOGIN_INIT_URL)
        .header(header::ACCEPT_LANGUAGE, "zh-CN")
        .send()
        .await
        .map_err(|e| format!("打开登录页失败：{e}"))?;
    let final_url = response.url().to_string();
    let html = response
        .text()
        .await
        .map_err(|e| format!("读取登录页失败：{e}"))?;
    let uuid = extract_uuid(&html)?;
    Ok((uuid, final_url))
}

async fn fetch_qr_image(client: &Client, uuid: &str, ts: &str, sig: &str) -> Result<Vec<u8>, String> {
    let response = client
        .get(JACCOUNT_QR_URL)
        .query(&[("uuid", uuid), ("ts", ts), ("sig", sig)])
        .send()
        .await
        .map_err(|e| format!("获取二维码失败：{e}"))?;
    if !response.status().is_success() {
        return Err(format!("二维码接口返回 {}", response.status()));
    }
    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|e| format!("读取二维码失败：{e}"))
}

fn jaccount_cookie_header(jar: &Jar) -> Result<String, String> {
    let url = Url::parse(JACCOUNT_WS_ORIGIN).map_err(|e| e.to_string())?;
    jar.cookies(&url)
        .and_then(|value| value.to_str().ok().map(str::to_string))
        .ok_or_else(|| "缺少 jAccount Cookie，请刷新二维码".into())
}

async fn complete_jaccount_login(client: &Client, uuid: &str) -> Result<(), String> {
    let response = client
        .get(JACCOUNT_EXPRESS_URL)
        .query(&[("uuid", uuid)])
        .header(header::ACCEPT_LANGUAGE, "zh-CN")
        .send()
        .await
        .map_err(|e| format!("完成扫码登录失败：{e}"))?;
    if response.url().as_str().starts_with(JACCOUNT_EXPRESS_URL) {
        return Err("扫码尚未确认，或登录已过期，请刷新二维码重试".into());
    }
    Ok(())
}

async fn finalize_canvas_login(client: &Client) -> Result<(), String> {
    let response = client
        .get(CANVAS_LOGIN_URL)
        .header(header::ACCEPT_LANGUAGE, "zh-CN")
        .send()
        .await
        .map_err(|e| format!("连接 Canvas 失败：{e}"))?;
    if !response.status().is_success() && response.status().as_u16() != 302 {
        return Err(format!("Canvas 登录返回 {}", response.status()));
    }
    Ok(())
}

fn emit_qr_code(app: &AppHandle, bytes: &[u8]) {
    let image = BASE64.encode(bytes);
    let _ = app.emit("qr-code-update", image);
}

fn emit_auth(app: &AppHandle, phase: &'static str, message: impl Into<String>) {
    let _ = app.emit(
        "auth-status",
        AuthStatus {
            phase,
            message: message.into(),
        },
    );
}

async fn request_qr_refresh(
    ws_tx: &mut (impl futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
) -> Result<(), String> {
    use futures_util::SinkExt;
    ws_tx
        .send(Message::Text(r#"{ "type": "UPDATE_QR_CODE" }"#.into()))
        .await
        .map_err(|e| format!("刷新二维码失败：{e}"))
}

pub async fn run_qr_login(
    app: AppHandle,
    jar: Arc<Jar>,
    refresh_rx: mpsc::Receiver<()>,
    mut stop_rx: oneshot::Receiver<()>,
) {
    emit_auth(&app, "qr-login", "正在获取二维码…");

    let client = match client_with_jar(jar.clone()) {
        Ok(client) => client,
        Err(message) => {
            emit_auth(&app, "error", message);
            return;
        }
    };

    let (uuid, referer) = match fetch_login_page(&client).await {
        Ok(result) => result,
        Err(message) => {
            emit_auth(&app, "error", message);
            return;
        }
    };

    let cookie_header = match jaccount_cookie_header(&jar) {
        Ok(cookie) => cookie,
        Err(message) => {
            emit_auth(&app, "error", message);
            return;
        }
    };

    let ws_url = format!("wss://jaccount.sjtu.edu.cn/jaccount/sub/{uuid}");
    let mut request = match ws_url.as_str().into_client_request() {
        Ok(request) => request,
        Err(error) => {
            emit_auth(&app, "error", format!("建立扫码连接失败：{error}"));
            return;
        }
    };
    request
        .headers_mut()
        .insert(header::COOKIE, header::HeaderValue::from_str(&cookie_header).unwrap());

    let (ws_stream, _) = match connect_async(request).await {
        Ok(stream) => stream,
        Err(error) => {
            emit_auth(&app, "error", format!("连接 jAccount 失败：{error}"));
            return;
        }
    };

    use futures_util::{SinkExt, StreamExt};
    let (mut ws_tx, mut ws_rx) = ws_stream.split();
    let mut refresh_rx = refresh_rx;

    if request_qr_refresh(&mut ws_tx).await.is_err() {
        emit_auth(&app, "error", "无法请求二维码，请重试");
        return;
    }

    emit_auth(&app, "qr-login", "请使用交我办扫码");

    loop {
        tokio::select! {
            _ = &mut stop_rx => break,
            refresh = refresh_rx.recv() => {
                if refresh.is_none() {
                    break;
                }
                let _ = request_qr_refresh(&mut ws_tx).await;
            }
            message = ws_rx.next() => {
                let Some(message) = message else { break; };
                let message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        emit_auth(&app, "error", format!("扫码连接中断：{error}"));
                        break;
                    }
                };
                if let Message::Text(text) = message {
                    let payload: Value = match serde_json::from_str(&text) {
                        Ok(value) => value,
                        Err(_) => continue,
                    };
                    match payload.get("type").and_then(Value::as_str) {
                        Some("UPDATE_QR_CODE") => {
                            let Some(body) = payload.get("payload") else { continue; };
                            let Some(ts) = body.get("ts").and_then(Value::as_str) else { continue; };
                            let Some(sig) = body.get("sig").and_then(Value::as_str) else { continue; };
                            match fetch_qr_image(&client, &uuid, ts, sig).await {
                                Ok(bytes) => emit_qr_code(&app, &bytes),
                                Err(message) => emit_auth(&app, "error", message),
                            }
                        }
                        Some("LOGIN") => {
                            emit_auth(&app, "qr-login", "扫码成功，正在连接 Canvas…");
                            if let Err(message) = complete_jaccount_login(&client, &uuid).await {
                                emit_auth(&app, "error", message);
                                break;
                            }
                            if let Err(message) = finalize_canvas_login(&client).await {
                                emit_auth(&app, "error", message);
                                break;
                            }
                            emit_auth(&app, "authorized", "Canvas 已连接");
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    let _ = referer;
}

pub fn spawn_qr_login(app: AppHandle, jar: Arc<Jar>) -> QrLoginController {
    let (refresh_tx, refresh_rx) = mpsc::channel(4);
    let (stop_tx, stop_rx) = oneshot::channel();
    tauri::async_runtime::spawn(run_qr_login(app, jar, refresh_rx, stop_rx));
    QrLoginController {
        refresh_tx,
        stop_tx: Some(stop_tx),
    }
}
