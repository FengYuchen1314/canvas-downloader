# Canvas@SJTU 课堂视频协议笔记

> 探测日期：2026-07-04。以下内容来自已登录用户可正常访问的新版“课堂视频”前端。
> 令牌、签名 URL 和个人信息不得写入日志或持久化。

## 已验证链路

1. 打开 `https://oc.sjtu.edu.cn` 的 Canvas 登录页；不能假设初始页面就是 jAccount。
2. 从 Canvas 登录页切换到“jAccount 校内用户登录”。
3. 在上海交大统一身份认证页使用“交我办”扫码登录。
4. 登录成功并返回 Canvas 控制面板。
5. 打开课程的新版课堂视频 LTI：
   `https://oc.sjtu.edu.cn/courses/{course_id}/external_tools/8329?display=borderless`。
6. Canvas 跳转到：
   `https://v.sjtu.edu.cn/jy-application-canvas-sjtu-ui/#/ivsModules/index?tokenId=...`。
7. 用短期 `tokenId` 换取视频服务访问令牌与课程参数。
8. 用课程参数获取讲次列表，再按讲次获取一到多路视频。
9. 使用服务端提供的官方下载接口下载每一路 MP4。

## 登录产品约束

应用只保留扫码登录，不提供账号密码或短信登录：

- 登录 WebView 必须识别 `Canvas 登录页 -> jAccount 统一认证页 -> Canvas 控制面板` 三个状态；
- 在 Canvas 登录页只暴露“jAccount 校内用户登录”入口；
- 进入 jAccount 后只展示“交我办”扫码区域，隐藏账号密码、短信登录及其切换入口；
- 应用不得读取、填写、传输或保存 jAccount 用户名、密码与短信验证码；
- 扫码确认完全由用户在“交我办”中完成；
- 若上游页面结构变化导致扫码入口无法识别，应提示重新加载或报告兼容性问题，不得回退到密码登录；
- 支持取消、刷新二维码和登录超时后重新扫码。

推荐将登录实现为显式状态机：

```text
CanvasLogin -> JAccountQrLogin -> CanvasAuthorized -> VideoLtiAuthorized
```

系统模型课程实测有 62 讲。抽查两讲均返回两路 1920×1080 MP4，时长约 55 分钟；
页面已确认两路标签分别为“教师”和“PPT”。当前未观察到 HLS 或 DRM，但其他课程仍需兼容单路、
多路和 HLS 返回值。

## API

Web UI：`https://v.sjtu.edu.cn/jy-application-canvas-sjtu-ui/`

API 根地址：`https://v.sjtu.edu.cn/jy-application-canvas-sjtu`

除令牌交换接口外，业务请求使用自定义请求头：

```http
token: <short-lived-token>
```

运行配置同时启用了 `withCredentials`。Tauri 客户端应保留视频域的会话 Cookie，但不要把令牌
写入磁盘。

### 1. 交换 LTI 令牌

```http
GET /lti3/getAccessTokenByTokenId?tokenId={tokenId}
```

前端把响应中的以下值放入内存状态：

- `data.token`：后续 API 的 `token` 请求头；
- `data.accessToken.jwt_token`：播放器相关 JWT；
- `data.params.courId`：列表接口的 `canvasCourseId`；
- `data.params.ltiCourseId`：统计与课程关联 ID；
- `data.params.courseName`：显示名称。

### 2. 获取讲次列表

```http
POST /directOnDemandPlay/findVodVideoList
Content-Type: application/json

{"canvasCourseId":"<URL-encoded courId>"}
```

主要响应字段：

```text
data.records[].videoId
data.records[].courseBeginTime
data.records[].courseEndTime
data.records[].videAuditStatus
```

学生端只应展示 `videAuditStatus == 3` 的开放录像。

### 3. 获取单讲媒体

```http
POST /directOnDemandPlay/getVodVideoInfos
Content-Type: multipart/form-data

playTypeHls=true
isAudit=true
id=<videoId>
```

主要响应字段：

```text
data.courId
data.lastWatchTime
data.videoPlayResponseVoList[].id
data.videoPlayResponseVoList[].cdviViewNum
data.videoPlayResponseVoList[].rtmpUrlHdv
```

`rtmpUrlHdv` 实际可为带临时 `key` 签名的 HTTPS MP4。`cdviViewNum` 的前端标签映射为：

```text
0 教师
1 学生1
2 学生2
3 PPT
4 合成
```

### 4. 官方下载接口

```http
GET /directOnDemandPlay/downloadVideo?id={Base64(id)}
```

- 单路录像：前端对讲次 `videoId` 做 Base64；
- 多路录像：前端分别对 `videoPlayResponseVoList[].id` 做 Base64，并逐路下载。

优先使用此接口，不长期保存 `rtmpUrlHdv` 的临时签名地址。实现断点续传前，需要继续验证
该接口或其重定向目标对 `Range`、`ETag` 和 `Content-Disposition` 的支持。

### 5. 获取 AI 字幕

```http
POST /transfer/translate/detail
Content-Type: application/json

{"courseId":"<getVodVideoInfos 返回的 data.courId>","platform":1}
```

主要响应字段：

```text
data.beforeAssemblyList[]
data.afterAssemblyList[].bg
data.afterAssemblyList[].ed
data.afterAssemblyList[].res
data.afterAssemblyList[].zh / en / ...
```

- `bg`、`ed` 的单位是毫秒；
- `res` 是默认识别文本；
- `zh`、`en` 等字段只在对应翻译已生成时出现；
- `afterAssemblyList` 适合导出字幕文件，`beforeAssemblyList` 用于播放时的细粒度同步；
- 字幕是独立 API 数据，不是 MP4 内嵌字幕或 HTML `<track>`。

第 02 讲实测返回 158 个合并字幕段，时间从 `00:00` 覆盖到 `54:46`。客户端应支持导出
UTF-8 SRT 和 WebVTT；字幕为 AI 生成，文件元数据或 UI 中应保留“仅供参考”的提示。

## Tauri 实现建议

- Tauri 2 + TypeScript UI + Rust `reqwest` 下载后端；
- 独立登录 WebView，只允许 `oc.sjtu.edu.cn`、`jaccount.sjtu.edu.cn` 和 `v.sjtu.edu.cn`；
- WebView UI 仅保留 jAccount 的“交我办”扫码流程，不渲染密码和短信登录选项；
- 监听跳转并从 URL fragment 中提取 `tokenId`，提取后立即从地址栏和日志中清除；
- Rust 内存中保存访问令牌，过期状态 `115117` 时要求重新登录；
- 先拉完整讲次列表，再并发获取媒体信息；下载并发默认 2，支持暂停、恢复与失败重试；
- 双路录像默认分别保存，后续可提供可选 FFmpeg 合成；
- 文件名建议：`{序号}-{日期}-{信号类型}.mp4`，字幕使用相同文件名前缀并保存为 `.srt`/`.vtt`。
